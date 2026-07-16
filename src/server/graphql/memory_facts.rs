//! The company-brain memory read: `Company.memory` over the [`FactStore`] port.

use std::sync::Arc;

use async_graphql::{Enum, ID, SimpleObject};

use super::iso8601;
use super::pagination::Page;
use crate::company::runtime::CompanyRuntime;
use crate::ports::facts::{FactKind, FactRecord};

/// The kind of a remembered fact, in the console's memory taxonomy.
#[derive(Enum, Copy, Clone, Eq, PartialEq)]
#[graphql(name = "MemoryKind")]
pub enum MemoryKindGql {
    /// A plain fact.
    Fact,
    /// An operator preference.
    Preference,
    /// A person the company knows.
    Person,
    /// A project the company tracks.
    Project,
    /// A reference document or link.
    Reference,
}

impl From<FactKind> for MemoryKindGql {
    fn from(kind: FactKind) -> Self {
        match kind {
            FactKind::Fact => Self::Fact,
            FactKind::Preference => Self::Preference,
            FactKind::Person => Self::Person,
            FactKind::Project => Self::Project,
            FactKind::Reference => Self::Reference,
        }
    }
}

impl From<MemoryKindGql> for FactKind {
    fn from(kind: MemoryKindGql) -> Self {
        match kind {
            MemoryKindGql::Fact => Self::Fact,
            MemoryKindGql::Preference => Self::Preference,
            MemoryKindGql::Person => Self::Person,
            MemoryKindGql::Project => Self::Project,
            MemoryKindGql::Reference => Self::Reference,
        }
    }
}

/// One remembered fact. Mirrors [`FactRecord`].
#[derive(SimpleObject)]
#[graphql(name = "MemoryFact")]
pub struct MemoryFactGql {
    /// The fact id.
    pub id: ID,
    /// The fact's kind.
    pub kind: MemoryKindGql,
    /// A short title.
    pub title: String,
    /// The fact body.
    pub body: String,
    /// Which desk/teammate captured it.
    pub source: String,
    /// When it was last updated, ISO-8601 UTC.
    pub updated_at: String,
}

impl From<FactRecord> for MemoryFactGql {
    fn from(record: FactRecord) -> Self {
        Self {
            id: ID(record.id),
            kind: record.kind.into(),
            title: record.title,
            body: record.body,
            source: record.source,
            updated_at: iso8601(record.updated_at_millis),
        }
    }
}

/// Resolves `Company.memory(query, kind, first, offset)`.
pub(crate) async fn resolve(
    runtime: &Arc<CompanyRuntime>,
    query: Option<String>,
    kind: Option<MemoryKindGql>,
    first: i32,
    offset: i32,
) -> async_graphql::Result<Page<MemoryFactGql>> {
    let rows = runtime
        .facts()
        .list(runtime.id(), query.as_deref(), kind.map(FactKind::from))
        .await?;
    let items: Vec<MemoryFactGql> = rows.into_iter().map(MemoryFactGql::from).collect();
    Ok(Page::slice(
        items,
        offset.max(0) as usize,
        first.max(0) as usize,
    ))
}
