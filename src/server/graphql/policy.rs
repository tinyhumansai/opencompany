//! The autonomy tier over GraphQL (issue #1070) — read-only.
//!
//! # Why this exists at all
//!
//! `Company` already resolves `approvals` and `pendingApprovals`, and did not
//! resolve the setting that decides whether anything ever *enters* that queue.
//! An empty `approvals` is either a healthy `supervised` company with nothing
//! pending or a company on `full` where nothing will ever park, and over
//! GraphQL alone those were indistinguishable.
//!
//! # Why it answers from the REST derivation rather than the record
//!
//! Every value here comes from
//! [`PolicyDto::build`](crate::server::ops::policy::PolicyDto::build), the same
//! function `GET {scope}/policy` answers with, which in turn reads
//! [`CompanyRecord::effective_policy`] — the resolver the approval gate itself
//! uses (`workflows::caps`, `harness`).
//!
//! Recomputing the tier here would have been three lines and a defect. A
//! company carrying a console override would report the overridden tier on one
//! surface and the manifest's on the other, with no way for a caller to know
//! which it received — the two-sources-of-truth failure issue #1027 nearly
//! shipped by adding this value to `/capabilities` instead. The `approvals`
//! field's own header states the same rule for redaction (issue #618): a
//! treatment applied to the REST handler alone leaves the boundary reachable
//! through one GraphQL field.
//!
//! # Read-only, deliberately
//!
//! There is no mutation. Changing the tier through an API is a separate and
//! much larger question, and #1027 disclaimed it for REST on the same grounds.
//! The write plane that does exist (`PUT`/`DELETE {scope}/policy`, issue #562)
//! is admin-gated and attributed; a GraphQL mutation would have to reproduce
//! both, which is not a thing to do as a side effect of closing a read gap.

use std::sync::Arc;

use async_graphql::SimpleObject;

use crate::runtime::CompanyRuntime;
use crate::server::ops::policy::PolicyDto;

/// One selectable tier and what choosing it means.
#[derive(Debug, SimpleObject)]
#[graphql(name = "PolicyTier")]
pub struct PolicyTierGql {
    /// The `[policy].mode` word.
    pub value: String,
    /// The operator-facing label.
    pub label: String,
    /// What choosing it means, in consequences rather than tier vocabulary.
    pub description: String,
}

/// The company's approval policy, as `GET {scope}/policy` reports it.
#[derive(Debug, SimpleObject)]
#[graphql(name = "Policy")]
pub struct PolicyGql {
    /// The tier **actually in force** — the console override where one sets it,
    /// the committed manifest otherwise.
    pub mode: String,
    /// The always-ask list actually in force. The operator's real lever: it
    /// wins over every tier, `full` included.
    pub always_approve: Vec<String>,
    /// The spend cap actually in force — console override where one sets it,
    /// the committed manifest otherwise. A higher cap is looser; `null` is the
    /// strictest setting — no spend is auto-approved, so every spend parks.
    pub auto_approve_under_usd: Option<f64>,
    /// How long an undecided approval remains actionable, in hours.
    pub approval_ttl_hours: f64,
    /// The manifest's tier, so a client can see what a reset would restore.
    pub manifest_mode: String,
    /// The manifest's always-ask list, for the same reason.
    pub manifest_always_approve: Vec<String>,
    /// The manifest's spend cap, for the same reason — what a reset would
    /// restore (a higher cap is looser; `null` is the strictest setting — no
    /// spend is auto-approved, so every spend parks).
    pub manifest_auto_approve_under_usd: Option<f64>,
    /// The manifest's approval deadline, for the same reason — what a reset
    /// would restore.
    pub manifest_approval_ttl_hours: Option<f64>,
    /// Whether an operator override is in force.
    ///
    /// Distinct from comparing `mode` with `manifestMode`: an override that
    /// happens to match the manifest is still an override, and is still what a
    /// reset would remove.
    pub overridden: bool,
    /// Who set the override, if one is set.
    pub set_by: Option<String>,
    /// When it was set (epoch millis), if one is set.
    pub set_at_millis: Option<f64>,
    /// The selectable tiers with their operator-facing consequences.
    pub tiers: Vec<PolicyTierGql>,
    /// When a change bites.
    pub takes_effect: String,
}

impl From<PolicyDto> for PolicyGql {
    fn from(dto: PolicyDto) -> Self {
        Self {
            mode: dto.mode,
            always_approve: dto.always_approve,
            auto_approve_under_usd: dto.auto_approve_under_usd,
            approval_ttl_hours: dto.approval_ttl_hours as f64,
            manifest_mode: dto.manifest_mode,
            manifest_always_approve: dto.manifest_always_approve,
            manifest_auto_approve_under_usd: dto.manifest_auto_approve_under_usd,
            manifest_approval_ttl_hours: dto.manifest_approval_ttl_hours.map(|hours| hours as f64),
            overridden: dto.overridden,
            set_by: dto.set_by,
            // GraphQL has no 64-bit integer scalar, and epoch millis exceed
            // `Int`. `f64` holds them exactly to 2^53, which is the year 287396
            // — the same widening `usage` already applies to its counters.
            set_at_millis: dto.set_at_millis.map(|ms| ms as f64),
            tiers: dto
                .tiers
                .into_iter()
                .map(|t| PolicyTierGql {
                    value: t.value.to_string(),
                    label: t.label.to_string(),
                    description: t.description.to_string(),
                })
                .collect(),
            takes_effect: dto.takes_effect.to_string(),
        }
    }
}

/// Loads the record and projects it through the REST derivation.
pub(crate) async fn resolve_policy(
    runtime: &Arc<CompanyRuntime>,
) -> async_graphql::Result<PolicyGql> {
    let record = runtime
        .store()
        .load(runtime.id())
        .await?
        .ok_or_else(|| async_graphql::Error::new("company not found"))?;
    Ok(PolicyDto::build(&record).into())
}
