//! The `Company` aggregation root and its directly-owned leaf objects.
//!
//! [`CompanyGql`] is a **handle**, not an eager projection: it carries the
//! company id and its [`CompanyRuntime`], and every field is an async resolver
//! that awaits the relevant port or parser only when selected. Nested fields
//! are safe without re-checking auth because the handle is only ever reachable
//! through an authorized `companies` / `company` query.

use std::sync::Arc;

use async_graphql::{Context, ID, Object, SimpleObject};

use super::auth::GqlAuth;
use super::connections::{ConnectionStateGql, DomainStatusGql, SmtpStatusGql};
use super::finances::FinancesGql;
use super::inbox::InboxGql;
use super::memory_facts::{MemoryFactGql, MemoryKindGql};
use super::observability;
use super::pagination::Page;
use super::policy;
use super::skills::SkillGql;
use super::tasks::TaskGql;
use super::usage::{UsageGql, UsageRangeGql};
use super::workflows::{WorkflowGql, WorkflowSummaryGql};
use super::workspace::{FsNodeGql, WorkspaceFileGql, WorkspaceSearchResultsGql};
use super::{
    connections, finances, inbox, memory_facts, skills, tasks, usage, workflows, workspace,
};
use crate::company::runtime::CompanyRuntime;
use crate::ports::types::Attachment;
use crate::ports::types::CompanyId;
use crate::ports::types::TurnStep;
use crate::server::chat_history::{self, MentionView, MessageView, ReactionView, Viewer};

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
    ///
    /// Contents are role-gated exactly as on the REST list (issue #618), and
    /// ownership is resolved exactly as there (#1891). This is the surface that
    /// made the question worth asking: it maps the same projection, so either
    /// applied only to the REST handlers would leave the whole boundary
    /// reachable through one GraphQL field — the redaction as a contents hole,
    /// and the raw park stamp as a client putting an approval on a card the
    /// host does not consider its owner.
    async fn approvals(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<ApprovalGql>> {
        let auth = ctx.data::<GqlAuth>()?;
        Ok(crate::server::approval_visibility::for_principal(
            auth,
            self.runtime.pending_approvals_resolved().await,
        )
        .into_iter()
        .map(ApprovalGql::from)
        .collect())
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

    /// Attempts at work — a card dispatch, a chat turn, or a workflow node —
    /// newest first, each with its step trace.
    ///
    /// `workflowRunId` is the join that had no answer before agent nodes minted
    /// rows: a node's turn has neither a card nor a conversation, so nothing
    /// could ask what a workflow run's agents did.
    ///
    /// The unredacted half of each step is **role-gated**: a member sees the
    /// scrubbed trace, and only a principal who may read sensitive contents sees
    /// the raw reasoning, arguments and output (`approval_visibility`).
    async fn agent_runs(
        &self,
        ctx: &Context<'_>,
        task_id: Option<ID>,
        workflow_run_id: Option<ID>,
        #[graphql(default = 50)] limit: i32,
    ) -> async_graphql::Result<Vec<observability::AgentRunGql>> {
        observability::resolve_runs(
            ctx,
            &self.runtime,
            task_id.map(|id| id.0),
            workflow_run_id.map(|id| id.0),
            limit,
        )
        .await
    }

    /// One attempt by id, with its step trace; null when absent.
    ///
    /// The unredacted half is role-gated exactly as on [`agent_runs`](Self::agent_runs).
    async fn agent_run(
        &self,
        ctx: &Context<'_>,
        id: ID,
    ) -> async_graphql::Result<Option<observability::AgentRunGql>> {
        observability::resolve_run(ctx, &self.runtime, id.0).await
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

    /// Which workspace notes mention `query`, matched case-insensitively as a
    /// substring of note names and note bodies (issue #607).
    async fn workspace_search(
        &self,
        query: String,
        prefix: Option<String>,
        limit: Option<i32>,
    ) -> async_graphql::Result<WorkspaceSearchResultsGql> {
        workspace::resolve_search(&self.runtime, &query, prefix.as_deref(), limit).await
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

    /// The company's saved workflows, as one-line summaries — seed graphs and
    /// runtime-authored ones alike. Each carries an `enabled` flag reporting
    /// manifest membership; listing is not gated on it.
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

    /// The approval policy in force (issue #1070).
    ///
    /// Answers from the same derivation `GET {scope}/policy` uses, so the two
    /// read surfaces cannot report different tiers for one company. Read-only:
    /// the write plane stays REST-and-admin-only (issue #562).
    ///
    /// This sits beside [`Self::approvals`] on purpose. That field says what is
    /// parked; this one says whether anything ever parks, and an empty queue
    /// means opposite things under `supervised` and `full`.
    async fn policy(&self) -> async_graphql::Result<policy::PolicyGql> {
        policy::resolve_policy(&self.runtime).await
    }

    /// The source-template provenance recorded at launch: the stable template
    /// id (directory slug) and, when known, its version. Null for a company
    /// provisioned from a raw manifest body rather than a template.
    async fn provenance(&self) -> async_graphql::Result<Option<TemplateProvenanceGql>> {
        let Some(record) = self.runtime.store().load(&self.id).await? else {
            return Ok(None);
        };
        Ok(record.template_provenance.map(|p| TemplateProvenanceGql {
            source_id: p.source_id,
            version: p.version,
            path: p.path,
        }))
    }
}

/// The source-template provenance of a company: where its manifest was seeded
/// from. Mirrors [`TemplateProvenance`](crate::ports::types::TemplateProvenance).
#[derive(SimpleObject)]
#[graphql(name = "TemplateProvenance")]
pub struct TemplateProvenanceGql {
    /// The template's stable identifier — the source directory slug.
    pub source_id: String,
    /// The template's version, when the source exposes one.
    pub version: Option<String>,
    /// The source directory path the company was launched from, when recorded.
    pub path: Option<String>,
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

        // Issue #304 — mirrored from the REST `list_team` deliberately: the two
        // reads of the same roster must not drift, so the cap columns are
        // resolved here by the same rule (one meter read, only when somebody is
        // capped; spend paired with the cap).
        // Issue #343: the scan is over EFFECTIVE caps across the whole roster
        // (manifest agents plus overlay teammates), because a console override
        // can cap somebody the manifest never did — including a teammate the
        // manifest does not contain at all.
        let any_capped = record
            .manifest
            .agents
            .iter()
            .map(|agent| &agent.id)
            .chain(record.overlay_agents.iter().map(|agent| &agent.id))
            .any(|id| record.effective_budget(id).is_some());
        let spend_today = if any_capped {
            let since = crate::metering::utc_day_start_millis(crate::ports::now_millis());
            Some(self.runtime.usage().query(&self.id, since).await?)
        } else {
            None
        };
        let spent = |id: &str| {
            spend_today
                .as_ref()
                .map(|samples| crate::metering::usd_spent_by_agent(samples, id))
        };

        // Issue #343: caps and their attribution resolve through the record for
        // BOTH arms, exactly as the REST handler does — an overlay teammate is
        // no longer hardcoded uncapped.
        let row = |id: &str, name: Option<String>, role: String, description: Option<String>| {
            let cap = record.effective_budget(id);
            let attribution = record.budget_override(id);
            TeamMemberGql {
                id: ID(id.to_string()),
                name,
                role,
                description,
                inbox_enabled: enabled(id),
                budget_usd_daily: cap,
                spent_today_usd: cap.and_then(|_| spent(id)),
                budget_set_by: attribution.map(|entry| entry.set_by.id.clone()),
                budget_set_at_millis: attribution.map(|entry| entry.at_millis as f64),
                // Resolved through the record, like the caps: one override row
                // answers for a manifest teammate and an overlay one alike, so
                // both arms of the roster get the chosen face with no second
                // lookup to keep in step (mirrored from the REST `list_team`).
                avatar: record.effective_avatar(id),
            }
        };
        // Resolved through the record for the same reason the caps are: a
        // manifest teammate an operator edited from the console reads back as
        // what it now is, here and in the REST `list_team`, identically.
        let effective = record.effective_agents();
        let mut out: Vec<TeamMemberGql> = effective
            .iter()
            .map(|agent| {
                row(
                    &agent.id,
                    agent.name.clone(),
                    agent.role.clone(),
                    agent.description.clone(),
                )
            })
            .collect();
        out.extend(record.overlay_agents.iter().map(|agent| {
            row(
                &agent.id,
                Some(agent.name.clone()),
                agent.role.clone(),
                agent.description.clone(),
            )
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
    /// Epoch-millis this approval default-denies if nobody decides it
    /// (issue #971). `Float` for the same reason as `atMillis`.
    ///
    /// Carried here rather than left to the REST summary alone so the two
    /// projections of one approval cannot drift into disagreeing about when it
    /// dies — which is the sort of divergence a reader discovers by acting on
    /// the wrong one. Null against a host that does not report a deadline.
    pub expires_at_millis: Option<f64>,
    /// Whether `amountUsd` was withheld from this reader by their role (#618).
    ///
    /// Carried for the same reason it is on the REST summary: a `null` amount
    /// otherwise means "this effect involves no money", and a Member looking at
    /// a withheld payment would read it as a free action.
    pub contents_hidden: bool,
    /// The workflow run waiting on this approval, when one is (issue #880).
    ///
    /// Structural — a run id, nothing more — and it is what lets a reader join
    /// a card on the Approvals page back to the run that opened it. Null for
    /// every park with no workflow behind it (a chat turn, a scheduler tick, a
    /// board task's attempt), which is the majority.
    pub workflow_run_id: Option<String>,
    /// Which workflow a parked `workflow.approve` gate is asking about
    /// (issue #1418), when the effect is one.
    ///
    /// Carried for the same reason it is on the REST summary: it is the second
    /// half of the run address, and it must survive role redaction or a Member
    /// holding up a stalled workflow would see `workflow_run_id` and still have
    /// nowhere to click.
    pub workflow_id: Option<String>,
}

impl From<crate::runtime::types::ApprovalSummary> for ApprovalGql {
    fn from(summary: crate::runtime::types::ApprovalSummary) -> Self {
        Self {
            id: ID(summary.id.as_ref().to_string()),
            kind: summary.kind,
            amount_usd: summary.amount_usd,
            at_millis: summary.at_millis as f64,
            expires_at_millis: summary.expires_at_millis.map(|ms| ms as f64),
            contents_hidden: summary.contents_hidden,
            workflow_run_id: summary.workflow_run_id,
            workflow_id: summary.workflow_id,
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
    /// This teammate's daily spend cap in force (issue #304), or null when it
    /// has none. Null-vs-set is the capped/uncapped distinction — `0` would
    /// mean "capped at nothing".
    ///
    /// Since #343 this is the **effective** cap: an operator override set from
    /// the console when one is stored, the manifest value otherwise.
    pub budget_usd_daily: Option<f64>,
    /// What this teammate has spent since 00:00 UTC; non-null only alongside a
    /// cap.
    pub spent_today_usd: Option<f64>,
    /// The user id of the admin who last set this teammate's cap from the
    /// console (issue #343); null when no override is stored. Non-null even
    /// when the override *removed* the cap, which is how "nobody capped this"
    /// is told from "an admin uncapped this".
    pub budget_set_by: Option<String>,
    /// When that cap was set (epoch millis). `Float` round-trips the full u64
    /// range that would overflow GraphQL's `Int`, matching `Approval.atMillis`.
    pub budget_set_at_millis: Option<f64>,
    /// The face this teammate wears, when somebody has chosen one — a
    /// `tiny:<flavour>` mascot or a `blob:<nodeId>` upload
    /// (`docs/spec/runtime/avatars.md`). Absent means **nobody has chosen**, and
    /// the console draws the mascot it hashes from the id. Mirrors the REST
    /// team DTO's `avatar`; both arms of the roster read answer the same way.
    pub avatar: Option<String>,
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
    ///
    /// Filtering + projection is shared with the REST `GET .../chat/history`
    /// route via [`chat_history::history_for_desk`] (issue #65) so the two
    /// surfaces can never disagree about a desk's transcript.
    async fn history(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: i32,
        before: Option<String>,
    ) -> async_graphql::Result<Page<MessageGql>> {
        let before_seq = before.as_deref().and_then(|c| c.parse::<u64>().ok());
        // `GqlAuth::Platform` (no person behind the credential) gets
        // `is_admin: true`, matching `Viewer::Operator`'s existing, already
        // unrestricted access to the rest of a company's history — an
        // admin-only row (issue #1781 review, Codex P1) is not a narrower case
        // than that.
        let (viewer, is_admin) = match ctx.data::<GqlAuth>() {
            Ok(GqlAuth::User(user)) => (
                Viewer::User(user.user_id.clone()),
                user.role.may_administer(),
            ),
            _ => (Viewer::Operator, true),
        };
        let first = first.clamp(0, chat_history::CHAT_HISTORY_PAGE_LIMIT as i32) as usize;
        let messages = chat_history::history_for_desk(
            &self.runtime,
            &self.desk.id,
            &self.desk.name,
            &viewer,
            before_seq,
            first,
            is_admin,
        )
        .await?;
        let total = chat_history::history_total_for_desk(
            &self.runtime,
            &self.desk.id,
            &self.desk.name,
            before_seq,
            is_admin,
        )
        .await?;
        Ok(Page {
            items: messages.into_iter().map(MessageGql::from).collect(),
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
    /// The scrubbed processing steps behind a company reply, so a rehydrated
    /// transcript renders the same timeline the REST route returns (issue #65
    /// parity). Empty for operator messages and tool-less replies.
    pub steps: Vec<MessageStepGql>,
    /// The board card this reply is about (issue #246): the card the turn
    /// opened, or the dispatched card it ran for (#185). Projected from the
    /// same [`MessageView`] field the REST route reads, so the two surfaces
    /// agree on which messages carry a card. Null on operator messages and on
    /// every reply journaled before the field existed.
    pub task_id: Option<ID>,
    /// The message this one replies to (issue #364) — a thread reply rather
    /// than a new line in the channel. Null for a message posted straight into
    /// the channel, which is every message journaled before threads were
    /// persisted. Same [`MessageView`] field the REST route reads, so the two
    /// surfaces cannot disagree about the shape of a thread.
    pub parent_id: Option<ID>,
    /// Who reacted to this message with what (issue #364), one row per person
    /// per emoji. Empty when nobody has.
    pub reactions: Vec<MessageReactionGql>,
    /// Who this message names, in reading order. Empty when it names nobody.
    /// Same [`MessageView`] field the REST route reads, so the two surfaces
    /// cannot disagree about who was mentioned.
    pub mentions: Vec<MessageMentionGql>,
    /// Files attached to this message (issue #1682), each a reference into the
    /// company workspace with the store-computed name / mime / size. Empty on a
    /// reply, a system pill, and every operator message journaled before the
    /// field existed. Same [`MessageView`] field the REST route reads, so the
    /// two surfaces carry the same rows (issue #65).
    pub attachments: Vec<MessageAttachmentGql>,
}

/// One mention on a history message. GraphQL mirror of the REST `mentions`
/// array, so the two surfaces carry the same rows.
#[derive(SimpleObject)]
#[graphql(name = "MessageMention")]
pub struct MessageMentionGql {
    /// The literal span the author typed.
    pub text: String,
    /// Byte offset of `text` in the message body.
    pub offset: u32,
    /// Who was named, as a display label — never a raw user id.
    pub label: String,
    /// Whether the reading viewer is the one named (or was named by
    /// `@everyone`).
    pub mine: bool,
    /// Whether this mention renders but pings nobody.
    pub quiet: bool,
}

/// One file attached to a history message (issue #1682). GraphQL mirror of the
/// REST `attachments` array, so the two surfaces carry the same rows. Every
/// field is store-authored metadata; the bytes are fetched separately through
/// the hardened `GET …/workspace/blob/{nodeId}` route.
#[derive(SimpleObject)]
#[graphql(name = "MessageAttachment")]
pub struct MessageAttachmentGql {
    /// The workspace node id the payload is stored under.
    pub node_id: ID,
    /// The stored file's display name.
    pub name: String,
    /// The stored payload's media type.
    pub mime: String,
    /// The stored payload's exact length in bytes.
    pub size: f64,
}

impl From<Attachment> for MessageAttachmentGql {
    fn from(attachment: Attachment) -> Self {
        MessageAttachmentGql {
            node_id: ID(attachment.node_id),
            name: attachment.name,
            mime: attachment.mime,
            // GraphQL has no 64-bit integer; `Float` carries a byte count
            // exactly up to 2^53, far past any file this route will store.
            size: attachment.size as f64,
        }
    }
}

impl From<MentionView> for MessageMentionGql {
    fn from(view: MentionView) -> Self {
        MessageMentionGql {
            text: view.text,
            // Saturating rather than `as`: GraphQL has no 64-bit integer, and a
            // silent wrap would put a chip at the wrong place in the text. An
            // offset this large cannot occur in a chat message, so the clamp is
            // unreachable — it is here so that if it ever is reached, the chip
            // lands at the end rather than at a wrapped-around position.
            //
            // `i32::MAX`, not `u32::MAX`: the GraphQL spec defines `Int` as a
            // signed 32-bit integer, so a `u32` value above `i32::MAX` is
            // already out of range for the scalar this field is declared as
            // and can fail serialization on the way out — the clamp exists
            // for exactly this unreachable-in-practice case, so it needs to
            // clamp to a value the wire type can actually carry.
            offset: i32::try_from(view.offset).unwrap_or(i32::MAX) as u32,
            label: view.label,
            mine: view.mine,
            quiet: view.quiet,
        }
    }
}

/// One person's reaction on a history message (issue #364). GraphQL mirror of
/// the REST `reactions` array, so the two surfaces carry the same rows.
#[derive(SimpleObject)]
#[graphql(name = "MessageReaction")]
pub struct MessageReactionGql {
    /// The emoji.
    pub emoji: String,
    /// Who reacted, as a display label — never a raw user id.
    pub by: String,
    /// Whether the reading viewer is the one who reacted.
    pub mine: bool,
}

impl From<ReactionView> for MessageReactionGql {
    fn from(view: ReactionView) -> Self {
        MessageReactionGql {
            emoji: view.emoji,
            by: view.by_label,
            mine: view.mine,
        }
    }
}

/// One scrubbed step in a reply's processing timeline. GraphQL mirror of the
/// wire [`TurnStep`] (`kind`/`status` are its snake_case string forms), so the
/// GraphQL `Message` type carries the same timeline as the REST projection.
#[derive(SimpleObject)]
#[graphql(name = "MessageStep")]
pub struct MessageStepGql {
    /// The step kind (`tool_call` / `thinking` / `note`).
    pub kind: String,
    /// How the step ended (`ok` / `error` / `running`).
    pub status: String,
    /// A short, human label (never tool arguments or output).
    pub label: String,
    /// An optional scrubbed detail (e.g. `server · tool`, a failure cause).
    pub detail: Option<String>,
    /// How long the step took in milliseconds, when known.
    pub elapsed_ms: Option<f64>,
}

impl From<TurnStep> for MessageStepGql {
    fn from(step: TurnStep) -> Self {
        // Reuse the serde snake_case forms so GraphQL and REST agree verbatim.
        let token = |v: &serde_json::Value| v.as_str().unwrap_or_default().to_string();
        MessageStepGql {
            kind: token(&serde_json::to_value(step.kind).unwrap_or_default()),
            status: token(&serde_json::to_value(step.status).unwrap_or_default()),
            label: step.label,
            detail: step.detail,
            elapsed_ms: step.elapsed_ms.map(|ms| ms as f64),
        }
    }
}

/// Wraps a viewer-agnostic [`MessageView`] (shared with the REST
/// `chat/history` route, issue #65) as the GraphQL `Message` type.
impl From<MessageView> for MessageGql {
    fn from(view: MessageView) -> Self {
        MessageGql {
            id: ID(view.id),
            channel: view.channel,
            author: view.author,
            text: view.text,
            at_millis: view.at_millis,
            mine: view.mine,
            steps: view.steps.into_iter().map(MessageStepGql::from).collect(),
            task_id: view.task_id.map(ID),
            parent_id: view.parent_id.map(ID),
            reactions: view
                .reactions
                .into_iter()
                .map(MessageReactionGql::from)
                .collect(),
            mentions: view
                .mentions
                .into_iter()
                .map(MessageMentionGql::from)
                .collect(),
            attachments: view
                .attachments
                .into_iter()
                .map(MessageAttachmentGql::from)
                .collect(),
        }
    }
}
