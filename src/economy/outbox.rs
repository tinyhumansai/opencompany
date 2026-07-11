//! An in-memory outbox of outbound tiny.place actions deferred while the
//! network is unreachable.
//!
//! When tiny.place cannot be reached, the [`TinyplaceEconomy`](super::adapter::
//! TinyplaceEconomy) queues the action here instead of failing boot or a cycle:
//! the published card goes stale, an outbound task waits, but the runtime keeps
//! moving. Draining and replaying the queue is the caller's responsibility.
//!
//! In-memory only this phase; a durable, cross-restart outbox is a documented
//! follow-up.

use std::sync::Mutex;

use crate::ports::types::{A2aTask, AgentAddr, AgentCard};

/// One deferred outbound action.
#[derive(Clone, Debug, PartialEq)]
pub enum OutboxAction {
    /// Publish (or refresh) the company's Agent Card.
    PublishCard(AgentCard),
    /// Claim or renew a `@handle` registration.
    Register {
        /// The `@handle` label to register.
        label: String,
    },
    /// Send an A2A task to a counterparty.
    SendTask {
        /// The counterparty address.
        to: AgentAddr,
        /// The task to deliver.
        task: A2aTask,
    },
}

/// A thread-safe FIFO queue of [`OutboxAction`]s.
#[derive(Default)]
pub struct Outbox {
    inner: Mutex<Vec<OutboxAction>>,
}

impl Outbox {
    /// Creates an empty outbox.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends an action to the back of the queue.
    pub fn enqueue(&self, action: OutboxAction) {
        self.inner.lock().expect("outbox poisoned").push(action);
    }

    /// Removes and returns every queued action, leaving the outbox empty.
    pub fn drain(&self) -> Vec<OutboxAction> {
        std::mem::take(&mut *self.inner.lock().expect("outbox poisoned"))
    }

    /// The number of actions currently queued.
    pub fn len(&self) -> usize {
        self.inner.lock().expect("outbox poisoned").len()
    }

    /// Whether the outbox holds no actions.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn enqueue_len_and_drain_round_trip() {
        let outbox = Outbox::new();
        assert!(outbox.is_empty());
        outbox.enqueue(OutboxAction::Register {
            label: "acme".into(),
        });
        outbox.enqueue(OutboxAction::SendTask {
            to: AgentAddr("peer".into()),
            task: A2aTask {
                skill: "seo.audit".into(),
                input: serde_json::json!({}),
            },
        });
        assert_eq!(outbox.len(), 2);

        let drained = outbox.drain();
        assert_eq!(drained.len(), 2);
        assert!(outbox.is_empty(), "drain empties the queue");
    }
}
