//! Lifetime task-cost reconciliation.
//!
//! Attempt usage is durable on `RunRecord`; planning usage is durable on the
//! task because planning deliberately has no run. This module is the only place
//! those sources are added and lineage is rolled up.

use std::collections::{HashMap, HashSet};

use crate::ports::runs::RunRecord;
use crate::ports::tasks::TaskRecord;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct CostEntry {
    pub key: String,
    pub at_millis: u64,
    pub label: String,
    pub amount_usd: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct CostTotal {
    pub own_usd: f64,
    pub total_usd: f64,
}

#[derive(Clone, Debug, Default)]
pub(super) struct TaskCosts {
    pub entries: HashMap<String, Vec<CostEntry>>,
    pub totals: HashMap<String, CostTotal>,
    pub run_ids: HashMap<String, HashSet<String>>,
}

/// Reconciles task-owned planning calls and every attempt, then rolls each
/// child's total into its parent. `live_run_usd` comes from the durable usage
/// meter and only advances an unsettled run beyond its last stored snapshot.
pub(super) fn reconcile(
    tasks: &[TaskRecord],
    runs: &[RunRecord],
    live_run_usd: &HashMap<String, f64>,
) -> TaskCosts {
    let task_ids: HashSet<&str> = tasks.iter().map(|task| task.id.as_str()).collect();
    let mut out = TaskCosts::default();

    for task in tasks {
        let entries = out.entries.entry(task.id.clone()).or_default();
        for (index, attempt) in task.planning_attempts.iter().enumerate() {
            if attempt.usage.cost_usd > 0.0 {
                entries.push(CostEntry {
                    key: format!("planning:{}:{index}", attempt.at_millis),
                    at_millis: attempt.at_millis,
                    label: "Planning pass".to_string(),
                    amount_usd: attempt.usage.cost_usd,
                });
            }
        }
    }

    for run in runs {
        let Some(task_id) = run.task_id.as_ref() else {
            continue;
        };
        if !task_ids.contains(task_id.as_str()) {
            continue;
        }
        out.run_ids
            .entry(task_id.clone())
            .or_default()
            .insert(run.id.clone());
        let amount_usd = live_run_usd
            .get(&run.id)
            .copied()
            .unwrap_or_default()
            .max(run.usage.cost_usd);
        if amount_usd <= 0.0 {
            continue;
        }
        out.entries
            .entry(task_id.clone())
            .or_default()
            .push(CostEntry {
                key: format!("run:{}", run.id),
                at_millis: run
                    .finished_at_millis
                    .or(run.started_at_millis)
                    .unwrap_or(run.created_at_millis),
                label: format!("Attempt {} · {}", run.attempt, run.status),
                amount_usd,
            });
    }

    for entries in out.entries.values_mut() {
        entries.sort_by(|a, b| {
            a.at_millis
                .cmp(&b.at_millis)
                .then_with(|| a.key.cmp(&b.key))
        });
    }

    let own: HashMap<String, f64> = tasks
        .iter()
        .map(|task| {
            let amount = out
                .entries
                .get(&task.id)
                .into_iter()
                .flatten()
                .map(|entry| entry.amount_usd)
                .sum();
            (task.id.clone(), amount)
        })
        .collect();
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    for task in tasks {
        if let Some(parent) = &task.parent_task_id
            && task_ids.contains(parent.as_str())
        {
            children
                .entry(parent.clone())
                .or_default()
                .push(task.id.clone());
        }
    }

    fn total_for(
        id: &str,
        own: &HashMap<String, f64>,
        children: &HashMap<String, Vec<String>>,
        visiting: &mut HashSet<String>,
        memo: &mut HashMap<String, f64>,
    ) -> f64 {
        if let Some(total) = memo.get(id) {
            return *total;
        }
        if !visiting.insert(id.to_string()) {
            return own.get(id).copied().unwrap_or_default();
        }
        let total = own.get(id).copied().unwrap_or_default()
            + children
                .get(id)
                .into_iter()
                .flatten()
                .map(|child| total_for(child, own, children, visiting, memo))
                .sum::<f64>();
        visiting.remove(id);
        memo.insert(id.to_string(), total);
        total
    }

    let mut memo = HashMap::new();
    for task in tasks {
        let total_usd = total_for(&task.id, &own, &children, &mut HashSet::new(), &mut memo);
        out.totals.insert(
            task.id.clone(),
            CostTotal {
                own_usd: own.get(&task.id).copied().unwrap_or_default(),
                total_usd,
            },
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::runs::{RunRecord, RunStatus};
    use crate::ports::tasks::{TaskDeliverable, TaskPlanningUsage};
    use crate::ports::types::{CompanyId, TokenUsage};

    fn task(id: &str, parent: Option<&str>, planning_cost: f64) -> TaskRecord {
        TaskRecord {
            id: id.to_string(),
            title: id.to_string(),
            note: None,
            column: "done".to_string(),
            priority: "low".to_string(),
            assignee: "ops".to_string(),
            updated_at_millis: 1,
            origin_chat_id: None,
            parent_task_id: parent.map(str::to_string),
            output: None,
            plan: None,
            planning_attempts: (planning_cost > 0.0)
                .then(|| TaskPlanningUsage {
                    at_millis: 2,
                    usage: TokenUsage {
                        cost_usd: planning_cost,
                        ..TokenUsage::default()
                    },
                })
                .into_iter()
                .collect(),
            deliverable: TaskDeliverable::Once,
            workflow_proposal: None,
            origin_run_id: None,
            origin_workflow_id: None,
            bounced: None,
        }
    }

    fn run(id: &str, task_id: &str, status: RunStatus, cost: f64) -> RunRecord {
        RunRecord {
            id: id.to_string(),
            company: CompanyId::new("acme"),
            task_id: Some(task_id.to_string()),
            chat_id: None,
            agent_id: "ops".to_string(),
            attempt: 1,
            status,
            trigger_event_seq: None,
            created_at_millis: 3,
            started_at_millis: Some(3),
            finished_at_millis: Some(4),
            error: None,
            usage: TokenUsage {
                cost_usd: cost,
                ..TokenUsage::default()
            },
            step_count: 0,
            workflow_run_id: None,
            node_id: None,
        }
    }

    #[test]
    fn task_total_always_equals_timeline_costs_plus_child_totals() {
        let tasks = vec![
            task("parent", None, 0.1),
            task("child", Some("parent"), 0.05),
        ];
        let runs = vec![
            // Failed attempts count exactly like successful ones.
            run("failed", "parent", RunStatus::Failed, 0.2),
            run("success", "child", RunStatus::Succeeded, 0.3),
        ];
        let costs = reconcile(&tasks, &runs, &HashMap::new());
        let parent = costs.totals["parent"];
        let own_timeline: f64 = costs.entries["parent"]
            .iter()
            .map(|entry| entry.amount_usd)
            .sum();
        let children = costs.totals["child"].total_usd;

        assert!((parent.total_usd - (own_timeline + children)).abs() < 1e-12);
        assert!((parent.total_usd - 0.65).abs() < 1e-12);
        assert!((parent.own_usd - 0.3).abs() < 1e-12);
    }

    #[test]
    fn zero_usage_creates_no_cost_line() {
        let tasks = vec![task("free", None, 0.0)];
        let runs = vec![run("free-run", "free", RunStatus::Succeeded, 0.0)];
        let costs = reconcile(&tasks, &runs, &HashMap::new());
        assert!(costs.entries["free"].is_empty());
        assert_eq!(costs.totals["free"].total_usd, 0.0);
    }

    #[test]
    fn redacted_task_cost_is_explicit_and_not_zero() {
        let display = super::super::tasks::CostDisplay::new(8.25, false).expect("positive cost");
        let value = serde_json::to_value(display).expect("serialize cost");
        assert_eq!(value["hidden"], true);
        assert!(value.get("amountUsd").is_none(), "{value}");
    }
}
