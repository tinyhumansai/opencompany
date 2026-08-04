//! [`HarnessRunTurn`]: the harness's [`RunTurn`] implementation.
//!
//! Wraps a [`HarnessPool`] and the [`HarnessDeps`] every turn draws on, and
//! forwards each [`RunTurn`] method to the matching `pool.run*` call — the one
//! place [`HarnessDeps`] is re-attached to the brain-agnostic delegation seam
//! (issue #176). Both are borrowed, so the shared delegation queue and steer
//! registry inside `deps` stay the very handles the
//! [`DelegationRunner`](crate::runtime::delegation::DelegationRunner) drains.
//!
//! Compiled only under `feature = "openhuman"`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::Result;
use crate::company::steer::SteerControl;
use crate::harness::run_trace::RunTraceSink;
use crate::harness::{HarnessDeps, HarnessPool, TurnOutcome};
use crate::ports::types::CompanyId;
use crate::runtime::delegation::RunTurn;

/// The harness's [`RunTurn`]: re-attaches [`HarnessDeps`] onto each pool turn.
pub struct HarnessRunTurn<'a> {
    pool: &'a HarnessPool,
    deps: &'a HarnessDeps,
}

impl<'a> HarnessRunTurn<'a> {
    /// Wraps a pool + deps for one cycle's worth of turns.
    pub fn new(pool: &'a HarnessPool, deps: &'a HarnessDeps) -> Self {
        Self { pool, deps }
    }
}

#[async_trait]
impl RunTurn for HarnessRunTurn<'_> {
    async fn run(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        chat_id: Option<&str>,
    ) -> Result<TurnOutcome> {
        self.pool
            .run(company, agent_id, message, self.deps, chat_id)
            .await
    }

    async fn run_steered(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        control: &SteerControl,
        chat_id: Option<&str>,
        run_sink: Option<Arc<RunTraceSink>>,
    ) -> Result<TurnOutcome> {
        self.pool
            .run_steered(
                company, agent_id, message, self.deps, control, chat_id, run_sink,
            )
            .await
    }

    async fn run_steered_background(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        control: &SteerControl,
        run_sink: Option<Arc<RunTraceSink>>,
    ) -> Result<TurnOutcome> {
        self.pool
            .run_steered_background(company, agent_id, message, self.deps, control, run_sink)
            .await
    }
}
