pub(super) use super::*;
pub(super) use crate::ports::tasks::TaskTitle;

pub(super) use std::collections::VecDeque;
pub(super) use std::sync::Mutex;

pub(super) use crate::ports::TaskStore;
pub(super) use crate::ports::tasks::{
    COLUMN_DONE, COLUMN_IN_PROGRESS, COLUMN_IN_REVIEW, COLUMN_PAUSED, COLUMN_PLANNING,
};
pub(super) use crate::ports::types::LedgerEntry;
pub(super) use crate::store::FsOps;

// ── harness ─────────────────────────────────────────────────────────────

/// One scripted turn: what the agent replies, what its turn queues onto the
/// shared delegation queue, and whether an operator cancels it mid-flight.
#[derive(Default)]
pub(super) struct Turn {
    pub(super) reply: String,
    pub(super) queues: Vec<Delegation>,
    pub(super) cancel: bool,
    /// Tool calls this turn tried to make and had parked for approval
    /// (issue #465), pushed onto the shared approval queue exactly as the
    /// real [`ApprovalPolicy`](crate::harness::policy::ApprovalPolicy) does.
    pub(super) parks: Vec<String>,
    /// Board writes this turn attempts **through the real tool boundary**
    /// ([`DelegationQueue::push_within_cap`]) rather than through
    /// [`queues`](Self::queues), which is the test escape hatch and bypasses
    /// both the cap and the #453 commitment.
    ///
    /// Issue #267 needs the boundary: the whole gate is that a `spawn_task`
    /// on a question turn is REFUSED in the model's own turn, and a fixture
    /// that pushed straight onto the queue could never observe the refusal.
    pub(super) tool_pushes: Vec<Delegation>,
    /// Desks this turn named that the tool REFUSED (issue #272 for the
    /// delegator, #176 for a member), recorded the way
    /// `DelegateToDeskTool` records them so the drain can report the
    /// attempt. A refusal never becomes a `Delegation`, so this is the only
    /// way a fixture can stand in for one.
    pub(super) refuses: Vec<String>,
    /// Workflows this turn authors inline with `create_workflow`, staged onto
    /// the shared [`WorkflowRefQueue`] *while the turn runs* — which is the
    /// only honest place for it (issue #678). A fixture that staged before
    /// the call would be wiped by the pre-turn clear, and one that staged
    /// after would skip the boundary the drain reads.
    pub(super) authors: Vec<TaskOutputWorkflow>,
    /// The in-turn spend halt this turn reports (issue #1032), standing in
    /// for the real [`SpendStopHook`](crate::harness::spend::SpendStopHook)
    /// firing. There is no way to arm the real hook here — these fixtures
    /// run no model — so this is how a test scripts "this teammate ran out
    /// of money mid-turn" and then asserts where that fact ends up.
    pub(super) spend_halt: Option<crate::harness::SpendHalt>,
    /// The budget pause this turn reports (issue #1846), standing in for
    /// `classify_turn` recognising a budget-exhausted `Err` from a real
    /// model turn. There is no way to arm that classification here either
    /// — these fixtures run no model — so this is how a test scripts "this
    /// teammate's turn ran out of inference credits" and then asserts the
    /// pause survives the delegation folds, including the nested one.
    pub(super) budget_paused: Option<crate::harness::BudgetPause>,
}

impl Turn {
    pub(super) fn reply(reply: &str) -> Self {
        Self {
            reply: reply.to_string(),
            ..Self::default()
        }
    }

    /// A turn that authors workflows inline, the way an operator asking
    /// "create a workflow named X" is answered (issue #678).
    pub(super) fn authoring(reply: &str, authors: Vec<TaskOutputWorkflow>) -> Self {
        Self {
            reply: reply.to_string(),
            authors,
            ..Self::default()
        }
    }

    pub(super) fn queueing(reply: &str, queues: Vec<Delegation>) -> Self {
        Self {
            reply: reply.to_string(),
            queues,
            ..Self::default()
        }
    }

    /// A turn that reaches for a board tool the way the model does — through
    /// the tool boundary, where it can be refused (issue #267).
    pub(super) fn tooling(reply: &str, tool_pushes: Vec<Delegation>) -> Self {
        Self {
            reply: reply.to_string(),
            tool_pushes,
            ..Self::default()
        }
    }

    pub(super) fn cancelled(reply: &str) -> Self {
        Self {
            reply: reply.to_string(),
            cancel: true,
            ..Self::default()
        }
    }

    /// A turn whose `delegate_to_desk` call was REFUSED at the tool
    /// boundary (issue #176): nothing is queued, and the desk it named is
    /// recorded for the drain to report.
    pub(super) fn refused(reply: &str, desks: &[&str]) -> Self {
        Self {
            reply: reply.to_string(),
            refuses: desks.iter().map(|d| d.to_string()).collect(),
            ..Self::default()
        }
    }

    /// A turn the in-turn spend brake halted (issue #1032): it replies with
    /// whatever it had, and reports the halt alongside.
    pub(super) fn spend_halted(reply: &str, agent: &str, spent_usd: f64, cap_usd: f64) -> Self {
        Self {
            reply: reply.to_string(),
            spend_halt: Some(crate::harness::SpendHalt {
                agent: agent.to_string(),
                spent_usd,
                cap_usd,
            }),
            ..Self::default()
        }
    }

    /// A turn that paused for lack of inference budget/credits (issue
    /// #1846): it replies with the actionable pause copy, and reports the
    /// pause alongside — the delegation-fold analogue of
    /// [`spend_halted`](Self::spend_halted).
    pub(super) fn budget_paused(reply: &str, agent: &str, summary: &str) -> Self {
        Self {
            reply: reply.to_string(),
            budget_paused: Some(crate::harness::BudgetPause {
                agent: agent.to_string(),
                summary: summary.to_string(),
            }),
            ..Self::default()
        }
    }

    /// A turn whose **first** tool call parked for approval, so it produced
    /// nothing: the reply is the agent saying it is blocked, not a result.
    /// This is the shape in the issue #465 report.
    pub(super) fn parked(reply: &str, tool: &str) -> Self {
        Self {
            reply: reply.to_string(),
            parks: vec![tool.to_string()],
            ..Self::default()
        }
    }
}

/// A [`RunTurn`] that plays a fixed script of turns and records who was
/// asked to run what, so a test can assert on the *sequence* of turns a
/// drain produced without a harness pool or a live model.
pub(super) struct ScriptedTurns {
    queue: DelegationQueue,
    /// The same handle the runner reads, so a parked call is visible to the
    /// settle exactly as it is in production (issue #465).
    approvals: ApprovalRequestQueue,
    script: Mutex<VecDeque<Turn>>,
    calls: Mutex<Vec<(String, String)>>,
    /// The board as it looked at the START of each turn, so a test can prove
    /// a card existed *while* an agent worked rather than only afterwards.
    board_at_turn: Mutex<Vec<Vec<(String, String)>>>,
    /// The delegation-chain bound the scripted tool boundary enforces
    /// (issue #176), standing in for `[tools].max_delegation_depth`. The
    /// production `DelegateToDeskTool` reads it off the live record; a
    /// scripted turn has no tool, so the depth a test runs under is set
    /// here.
    max_depth: usize,
    /// How the delegation queue was **claimed** while each turn ran (issues
    /// #453, #267). This is what a real tool reads to decide between
    /// staging and refusing, so recording it here is how a test proves the
    /// turn was entitled to delegate at all — rather than only that the
    /// drain happened to run afterwards. Since #267's review it also
    /// distinguishes the narrowed answering claim from the full one.
    committed_at_turn: Mutex<Vec<orchestrator::DrainClaim>>,
    /// The ambient [`is_chat_only_turn`] hint read from INSIDE each turn,
    /// so a test proves the greeting fast path fired through the real
    /// classification path (`handle_operator_message`) rather than the
    /// caller forcing the scope directly (issue #1725 review — the
    /// original end-to-end test only ever asserted the hint by wrapping
    /// the call in `with_chat_only_hint(true, ..)` itself, which cannot
    /// catch the classifier failing to derive it).
    chat_only_at_turn: Mutex<Vec<bool>>,
    pub(super) history_seed_at_turn: Mutex<Vec<bool>>,
    /// What the tool boundary answered for each
    /// [`Turn::tool_pushes`] entry, in order across all turns (issue #267).
    staged: Mutex<Vec<orchestrator::Staged>>,
    tasks: Arc<dyn TaskStore>,
    company: CompanyId,
    /// The same shared handle the runner drains, so a scripted turn stages a
    /// workflow exactly where `CreateWorkflowTool` does (issue #678).
    workflow_refs: WorkflowRefQueue,
}

impl ScriptedTurns {
    pub(super) fn new(fx: &Fixture, turns: Vec<Turn>) -> Self {
        Self {
            queue: fx.queue.clone(),
            approvals: fx.approvals.clone(),
            script: Mutex::new(turns.into()),
            calls: Mutex::new(Vec::new()),
            board_at_turn: Mutex::new(Vec::new()),
            committed_at_turn: Mutex::new(Vec::new()),
            chat_only_at_turn: Mutex::new(Vec::new()),
            history_seed_at_turn: Mutex::new(Vec::new()),
            staged: Mutex::new(Vec::new()),
            tasks: fx.tasks.clone(),
            company: fx.record.id.clone(),
            workflow_refs: fx.workflow_refs.clone(),
            max_depth: usize::from(crate::company::DEFAULT_MAX_DELEGATION_DEPTH),
        }
    }

    /// Runs this script under a different `[tools].max_delegation_depth`
    /// (issue #176) — `1` reproduces the pre-#176 "desks may not
    /// re-delegate" behaviour.
    pub(super) fn with_max_depth(mut self, max_depth: usize) -> Self {
        self.max_depth = max_depth;
        self
    }

    /// What the tool boundary answered every [`Turn::tool_pushes`] call, in
    /// order (issue #267).
    pub(super) fn staged(&self) -> Vec<orchestrator::Staged> {
        self.staged.lock().expect("staged").clone()
    }

    /// `(agent_id, message)` for every turn run, in order.
    pub(super) fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().expect("calls").clone()
    }

    /// `(assignee, column)` for every card on the board when turn `n`
    /// started.
    pub(super) fn board_at_turn(&self, n: usize) -> Vec<(String, String)> {
        self.board_at_turn.lock().expect("board")[n].clone()
    }

    /// Whether the delegation queue was claimed at all while turn `n` ran
    /// (issue #453).
    pub(super) fn committed_at_turn(&self, n: usize) -> bool {
        self.claim_at_turn(n) != orchestrator::DrainClaim::Unclaimed
    }

    /// *How* the delegation queue was claimed while turn `n` ran — full, or
    /// narrowed to answering (issue #267).
    pub(super) fn claim_at_turn(&self, n: usize) -> orchestrator::DrainClaim {
        self.committed_at_turn.lock().expect("committed")[n]
    }

    /// Whether [`is_chat_only_turn`] read `true` from INSIDE turn `n` — the
    /// real hint the harness pool would have read, not one the test forced.
    pub(super) fn chat_only_at_turn(&self, n: usize) -> bool {
        self.chat_only_at_turn.lock().expect("chat_only")[n]
    }

    pub(super) async fn next(
        &self,
        agent_id: &str,
        message: &str,
        control: Option<&SteerControl>,
    ) -> TurnOutcome {
        self.calls
            .lock()
            .expect("calls")
            .push((agent_id.to_string(), message.to_string()));
        let board = self
            .tasks
            .list(&self.company)
            .await
            .expect("list cards")
            .into_iter()
            .map(|c| (c.assignee, c.column))
            .collect();
        self.board_at_turn.lock().expect("board").push(board);
        self.committed_at_turn
            .lock()
            .expect("committed")
            .push(self.queue.claim_state());
        self.chat_only_at_turn
            .lock()
            .expect("chat_only")
            .push(is_chat_only_turn());
        let turn = self
            .script
            .lock()
            .expect("script")
            .pop_front()
            .unwrap_or_else(|| panic!("unscripted turn: {agent_id} <- {message}"));
        for delegation in turn.queues {
            self.queue.push(delegation);
        }
        // …and the ones that go through the boundary a real tool goes
        // through, recording what it answered (issue #267).
        for delegation in turn.tool_pushes {
            let staged = self.queue.push_within_cap(
                delegation,
                orchestrator::MAX_DELEGATIONS_PER_TURN,
                self.max_depth,
            );
            self.staged.lock().expect("staged").push(staged);
        }
        // …and the ones the tool refused outright, which never become a
        // `Delegation` at all (issues #272, #176).
        for desk in turn.refuses {
            self.queue.push_refusal(desk);
        }
        // Staged mid-turn, like the inline `create_workflow` tool (#678).
        for authored in turn.authors {
            self.workflow_refs.push(authored);
        }
        for tool in turn.parks {
            self.approvals
                .push(crate::harness::policy::ApprovalRequest {
                    tool: tool.clone(),
                    reason: "supervised".to_string(),
                    effect: crate::ports::types::Effect {
                        kind: tool,
                        group: crate::ports::types::EffectGroup::Other,
                        amount_usd: None,
                        established_thread: false,
                        first_time_counterparty: false,
                        payload: serde_json::json!({}),
                        agent: Some(agent_id.to_string()),
                        run_id: None,
                    },
                });
        }
        if turn.cancel
            && let Some(control) = control
        {
            control.request(SteerAction::Cancel);
        }
        TurnOutcome {
            reply: turn.reply,
            steps: Vec::new(),
            // These fixtures script delegation shapes, not cap behaviour;
            // the cap path is proved end-to-end in `cap_turn_test`.
            hit_iteration_cap: false,
            // Scripted delegation fixture, not the ACP fold — the only
            // path that produces an abnormal stop (PR #1880 review).
            abnormal_stop: None,
            // Issue #1032: scripted, for the same reason — the real hook
            // needs a real model turn to fire, which is proved end-to-end
            // in `spend_halt_turn_test`. What these fixtures can prove, and
            // that one cannot, is that the halt survives the DELEGATION
            // folds, including the nested one.
            halted_for_spend: turn.spend_halt,
            // Issue #1846: scripted the same way, for the same reason —
            // `classify_turn` needs a real model `Err` to classify, which is
            // proved end-to-end elsewhere. What this fixture proves is that
            // a budget pause survives the DELEGATION folds, including the
            // nested one, exactly like a spend halt.
            budget_paused: turn.budget_paused,
        }
    }
}

#[async_trait]
impl RunTurn for ScriptedTurns {
    async fn run(
        &self,
        _company: &CompanyId,
        agent_id: &str,
        message: &str,
        _chat_id: ChatTarget<'_>,
    ) -> Result<TurnOutcome> {
        self.history_seed_at_turn
            .lock()
            .unwrap()
            .push(_chat_id.history_seed);
        Ok(self.next(agent_id, message, None).await)
    }

    async fn run_steered(
        &self,
        _company: &CompanyId,
        agent_id: &str,
        message: &str,
        control: &SteerControl,
        _chat_id: ChatTarget<'_>,
        _run_sink: Option<Arc<RunTraceSink>>,
    ) -> Result<TurnOutcome> {
        self.history_seed_at_turn
            .lock()
            .unwrap()
            .push(_chat_id.history_seed);
        Ok(self.next(agent_id, message, Some(control)).await)
    }

    async fn run_steered_background(
        &self,
        _company: &CompanyId,
        agent_id: &str,
        message: &str,
        control: &SteerControl,
        _chat: ChatTarget<'_>,
        _run_sink: Option<Arc<RunTraceSink>>,
    ) -> Result<TurnOutcome> {
        self.history_seed_at_turn
            .lock()
            .unwrap()
            .push(_chat.history_seed);
        Ok(self.next(agent_id, message, Some(control)).await)
    }
}

/// `chief` is the orchestrator; `engineer` leads the `eng_desk` desk.
pub(super) fn record() -> CompanyRecord {
    let manifest = toml::from_str(
        r#"
[company]
name = "Acme"

[[agent]]
id = "chief"
role = "Chief of Staff"
tier = "orchestrator"

[[agent]]
id = "engineer"
role = "Engineer"

[[group_chat]]
id = "eng_desk"
name = "Engineering"
members = ["engineer"]
"#,
    )
    .expect("valid manifest");
    CompanyRecord {
        overlay_desk_hive: Vec::new(),
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        id: CompanyId::new("acme"),
        manifest,
        ledger: Vec::<LedgerEntry>::new(),
        lifecycle: "running".to_string(),
        overlay_agents: Vec::new(),
        overlay_desk_members: Vec::new(),
        overlay_desk_order: Vec::new(),
        overlay_desks: Vec::new(),
        overlay_workflows: Vec::new(),
        overlay_budgets: Vec::new(),
        overlay_policy: None,
        overlay_tool_grants: None,
        overlay_desk_tools: Default::default(),
        disabled_workflows: Vec::new(),
        template_provenance: None,
        setup: None,
        name_confirmed: false,
        activation_completed_at: None,
        created_at_millis: None,
    }
}

/// A roster with **two** delegatable desks, where the engineering lead may
/// hand a slice on to research (issue #176).
///
/// `design` is deliberately outside `engineer`'s allowlist and led by a
/// third teammate, so a test can prove a third lead never runs.
pub(super) fn nested_record() -> CompanyRecord {
    let manifest = toml::from_str(
        r#"
[company]
name = "Acme"

[[agent]]
id = "chief"
role = "Chief of Staff"
tier = "orchestrator"

[[agent]]
id = "engineer"
role = "Engineer"
delegates_to = ["research_desk"]

[[agent]]
id = "researcher"
role = "Researcher"
delegates_to = ["design_desk"]

[[agent]]
id = "designer"
role = "Designer"

[[group_chat]]
id = "eng_desk"
name = "Engineering"
members = ["engineer"]

[[group_chat]]
id = "research_desk"
name = "Research"
members = ["researcher"]

[[group_chat]]
id = "design_desk"
name = "Design"
members = ["designer"]
"#,
    )
    .expect("valid manifest");
    CompanyRecord {
        manifest,
        ..record()
    }
}

/// The hand-off the engineering lead makes one level down (issue #176).
pub(super) fn nested_handoff(instruction: &str) -> Delegation {
    Delegation::DelegateToDesk {
        desk: "research_desk".to_string(),
        instruction: instruction.to_string(),
    }
}

/// The company shape issue #884 D1 was observed on: ONE desk with three
/// members, so the lead has peers beside it that `delegate_to_desk` — which
/// only ever resolves to the lead — could never reach.
pub(super) fn peer_record() -> CompanyRecord {
    let manifest = toml::from_str(
        r#"
[company]
name = "Acme"

[[agent]]
id = "chief"
role = "Chief of Staff"
tier = "orchestrator"

[[agent]]
id = "brand_strategist"
role = "Brand Strategist"

[[agent]]
id = "seo_specialist"
role = "SEO Specialist"

[[agent]]
id = "copywriter"
role = "Copywriter"

[[group_chat]]
id = "strategy"
name = "Strategy desk"
members = ["brand_strategist", "seo_specialist", "copywriter"]
"#,
    )
    .expect("valid manifest");
    CompanyRecord {
        manifest,
        ..record()
    }
}

/// A hand-off to a named teammate (issue #884).
pub(super) fn peer_handoff(teammate: &str, instruction: &str) -> Delegation {
    Delegation::DelegateToTeammate {
        teammate: teammate.to_string(),
        instruction: instruction.to_string(),
    }
}

/// The wired pieces one drain needs: the company record, a real task store
/// over a temp dir, the shared queue, and the steer registry.
pub(super) struct Fixture {
    _dir: tempfile::TempDir,
    pub(super) record: CompanyRecord,
    pub(super) tasks: Arc<dyn TaskStore>,
    pub(super) queue: DelegationQueue,
    steer: InflightRegistry,
    /// Wired into every runner, so the parked-approval overlay (issue #465)
    /// is exercised by the whole existing suite rather than only by the
    /// tests that park something.
    pub(super) approvals: ApprovalRequestQueue,
    /// Workflows a turn authored inline (issue #678). Empty in every test
    /// that does not stage one, which is what keeps this a pure addition.
    pub(super) workflow_refs: WorkflowRefQueue,
}

impl Fixture {
    pub(super) fn new() -> Self {
        Self::over(record())
    }

    /// A fixture over a three-desk roster whose leads may re-delegate
    /// (issue #176).
    pub(super) fn nested() -> Self {
        Self::over(nested_record())
    }

    /// A fixture over the one three-person desk issue #884 D1 was seen on.
    pub(super) fn peers() -> Self {
        Self::over(peer_record())
    }

    pub(super) fn over(record: CompanyRecord) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        Self {
            tasks: Arc::new(FsOps::new(dir.path())) as Arc<dyn TaskStore>,
            _dir: dir,
            record,
            queue: DelegationQueue::default(),
            steer: InflightRegistry::default(),
            approvals: ApprovalRequestQueue::default(),
            workflow_refs: WorkflowRefQueue::default(),
        }
    }

    pub(super) fn runner<'a>(&'a self, turns: &'a dyn RunTurn) -> DelegationRunner<'a> {
        DelegationRunner::new(
            turns,
            &self.record,
            Some(&self.tasks),
            &self.steer,
            &self.record.id,
            &self.queue,
            orchestrator::MAX_DELEGATIONS_PER_TURN,
        )
        .with_approvals(&self.approvals)
        .with_workflow_refs(&self.workflow_refs)
    }

    pub(super) async fn cards(&self) -> Vec<TaskRecord> {
        self.tasks.list(&self.record.id).await.expect("list cards")
    }
}

pub(super) fn handoff(instruction: &str) -> Delegation {
    Delegation::DelegateToDesk {
        desk: "eng_desk".to_string(),
        instruction: instruction.to_string(),
    }
}

// ── Issue #678: the escalation, and where it may not reach ──────────────

/// A scripted escalation. Records what it was asked so a test can prove the
/// model was *not* consulted on messages the cheap layer already named.
pub(super) struct ScriptedTriage {
    pub(super) verdict: crate::harness::triage::TriageVerdict,
    pub(super) asked: Mutex<Vec<String>>,
}
