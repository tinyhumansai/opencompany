//! The [`EventLog`] port: append-only, replayable event history.

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::Result;
use crate::ports::types::{CompanyEvent, CompanyId, EventSeq, StoredEvent};

/// Append-only, replayable event log. Boot replays the tail to rebuild
/// in-flight state.
#[async_trait]
pub trait EventLog: Send + Sync {
    /// Appends an event, returning its assigned sequence number.
    async fn append(&self, id: &CompanyId, event: CompanyEvent) -> Result<EventSeq>;
    /// Reads up to `limit` events with sequence `>= seq`.
    async fn read_from(
        &self,
        id: &CompanyId,
        seq: EventSeq,
        limit: usize,
    ) -> Result<Vec<StoredEvent>>;
    /// Subscribes to events appended after the call.
    fn subscribe(&self, id: &CompanyId) -> BoxStream<'static, StoredEvent>;
}
