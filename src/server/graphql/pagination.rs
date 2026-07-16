//! Offset/limit pagination wrapper shared by the list-shaped reads.
//!
//! The console renders full lists, so this is deliberately **not** Relay:
//! [`Page`] carries the requested slice and the unpaginated `total`. Each
//! concrete instantiation is registered with an explicit SDL name
//! (`TaskPage`, `MemoryFactPage`, `EmailPage`, `MessagePage`). The one cursor
//! exception — `Chat.history`'s opaque `before` — is an argument, not a page
//! shape, so it lives on the field, not here.

use async_graphql::{OutputType, SimpleObject};

use super::company::MessageGql;
use super::inbox::EmailMessageGql;
use super::memory_facts::MemoryFactGql;
use super::tasks::TaskGql;

/// A single page of `T`: the requested slice plus the unpaginated total.
#[derive(SimpleObject)]
#[graphql(concrete(name = "TaskPage", params(TaskGql)))]
#[graphql(concrete(name = "MemoryFactPage", params(MemoryFactGql)))]
#[graphql(concrete(name = "EmailPage", params(EmailMessageGql)))]
#[graphql(concrete(name = "MessagePage", params(MessageGql)))]
pub struct Page<T: OutputType> {
    /// The items on this page.
    pub items: Vec<T>,
    /// The total number of items across all pages (before offset/limit).
    pub total: i32,
}

impl<T: OutputType> Page<T> {
    /// Wraps a fully-materialized list, applying `offset`/`limit` and recording
    /// the pre-slice `total`. `total` is the length of `all` before slicing.
    pub fn slice(all: Vec<T>, offset: usize, limit: usize) -> Self {
        let total = all.len() as i32;
        let items = all.into_iter().skip(offset).take(limit).collect();
        Self { items, total }
    }
}
