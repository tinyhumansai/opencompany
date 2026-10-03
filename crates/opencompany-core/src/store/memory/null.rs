//! The explicit "no memory" engine behind `OPENCOMPANY_MEMORY=null`.
//!
//! TinyMemory v1 shipped a `NullMemoryProvider`; v2's registry builds only real
//! engines, so the host keeps its own. It is deliberately not a fallback:
//! nothing selects it except an operator naming `null`, and the overlay that
//! binds it warns once at open that nothing will be remembered.

use tinymemory::{
    EngineDescriptor, EngineHealth, FetchPage, FetchRequest, ForgetReport, ForgetTarget, ListPage,
    ListRequest, MemoryEngine, RecallAnswer, RecallRequest, Result, StoreItem, StoreReceipt,
    async_trait,
};

/// The engine id the null engine reports, and the catalog's "No memory" tile.
pub const NULL_ENGINE_ID: &str = "null";

/// Accepts every write and discards it; every read is empty.
///
/// Writes still validate, so a malformed item is refused here exactly as a
/// real engine would refuse it — a company that later switches to a real
/// engine must not discover that its writes were never well-formed.
#[derive(Debug)]
pub struct NullEngine {
    descriptor: EngineDescriptor,
}

impl Default for NullEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl NullEngine {
    /// A null engine. It advertises no fetch modes: there is nothing to rank.
    pub fn new() -> Self {
        Self {
            descriptor: EngineDescriptor {
                id: NULL_ENGINE_ID,
                label: "No memory",
                description: "Accepts writes and discards them; every read is empty.",
                hosted: false,
                needs_endpoint: false,
                needs_key: false,
                default_endpoint: None,
                fetch_modes: Vec::new(),
            },
        }
    }
}

#[async_trait]
impl MemoryEngine for NullEngine {
    fn descriptor(&self) -> &EngineDescriptor {
        &self.descriptor
    }

    async fn health(&self) -> EngineHealth {
        EngineHealth::Ok
    }

    async fn recall(&self, req: RecallRequest) -> Result<RecallAnswer> {
        req.validate()?;
        Ok(RecallAnswer {
            answer: String::new(),
            citations: Vec::new(),
            model: None,
        })
    }

    async fn fetch(&self, req: FetchRequest) -> Result<FetchPage> {
        self.descriptor.ensure_mode(req.mode)?;
        req.validate()?;
        Ok(FetchPage::default())
    }

    async fn store(&self, item: StoreItem) -> Result<StoreReceipt> {
        item.validate()?;
        Ok(StoreReceipt {
            id: item.fingerprint().into(),
            replayed: false,
        })
    }

    async fn forget(&self, target: ForgetTarget) -> Result<ForgetReport> {
        target.validate()?;
        Ok(ForgetReport::default())
    }

    async fn list(&self, req: ListRequest) -> Result<ListPage> {
        req.validate()?;
        Ok(ListPage::default())
    }
}

#[cfg(test)]
#[path = "null_tests.rs"]
mod tests;
