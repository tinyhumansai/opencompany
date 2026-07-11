//! [`RuntimeBuilder`]: wires a [`CompanyRuntime`] from filesystem defaults.
//!
//! `fs_defaults` assembles the Phase-1 stack — fs-backed stores, the
//! manifest-`[policy]` [`ManifestApprovalGate`](crate::policy::ManifestApprovalGate),
//! the offline [`EchoBrain`], a built-in operator channel, and the stub tool
//! provider — with no agent economy. Operators swap any port through the
//! `with_*` setters before [`build`](RuntimeBuilder::build).
//!
//! `build` performs boot replay: it loads the runtime journal and rehydrates
//! any parked approvals into the gate so an approval survives a restart.

use std::path::PathBuf;
use std::sync::Arc;

use crate::Result;
use crate::brain::EchoBrain;
use crate::company::CompanyManifest;
use crate::company::runtime::CompanyRuntime;
use crate::policy::ManifestApprovalGate;
use crate::ports::types::{CompanyId, CompanyRecord};
use crate::ports::{
    AgentEconomy, ApprovalGate, Brain, ChannelAdapter, CompanyStore, ContextStore, EventLog,
    MemoryStore, ToolProvider,
};
use crate::runtime::channel::OperatorChannel;
use crate::runtime::journal::RuntimeJournal;
use crate::runtime::tools::StubToolProvider;
use crate::store::paths::Bundle;
use crate::store::{FsCompanyStore, FsContextStore, FsEventLog, FsMemoryStore};

/// Derives a filesystem-and-URL-safe company id from a display name.
///
/// Lowercases, collapses runs of non-alphanumeric characters into single
/// hyphens, and trims leading/trailing hyphens (`"Acme Co!"` → `"acme-co"`).
pub fn company_id_from_name(name: &str) -> CompanyId {
    let mut slug = String::with_capacity(name.len());
    let mut prev_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    let trimmed = slug.trim_matches('-');
    CompanyId::new(if trimmed.is_empty() {
        "company"
    } else {
        trimmed
    })
}

/// Builds one company's [`CompanyRuntime`] over a filesystem home.
pub struct RuntimeBuilder {
    home: PathBuf,
    id: CompanyId,
    manifest: CompanyManifest,
    brain: Option<Arc<dyn Brain>>,
    store: Option<Arc<dyn CompanyStore>>,
    events: Option<Arc<dyn EventLog>>,
    memory: Option<Arc<dyn MemoryStore>>,
    context: Option<Arc<dyn ContextStore>>,
    tools: Option<Arc<dyn ToolProvider>>,
    channels: Option<Vec<Arc<dyn ChannelAdapter>>>,
    economy: Option<Arc<dyn AgentEconomy>>,
    approvals: Option<Arc<ManifestApprovalGate>>,
}

impl RuntimeBuilder {
    /// Starts a builder for `manifest` rooted at the OpenCompany home `home`.
    ///
    /// The company id defaults to a slug of the manifest name; override it with
    /// [`with_id`](Self::with_id).
    pub fn new(home: impl Into<PathBuf>, manifest: CompanyManifest) -> Self {
        let id = company_id_from_name(&manifest.company.name);
        Self {
            home: home.into(),
            id,
            manifest,
            brain: None,
            store: None,
            events: None,
            memory: None,
            context: None,
            tools: None,
            channels: None,
            economy: None,
            approvals: None,
        }
    }

    /// Overrides the derived company id.
    pub fn with_id(mut self, id: CompanyId) -> Self {
        self.id = id;
        self
    }

    /// Swaps the cognition brain (default [`EchoBrain`]).
    pub fn with_brain(mut self, brain: Arc<dyn Brain>) -> Self {
        self.brain = Some(brain);
        self
    }

    /// Swaps the company store.
    pub fn with_store(mut self, store: Arc<dyn CompanyStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Swaps the event log.
    pub fn with_events(mut self, events: Arc<dyn EventLog>) -> Self {
        self.events = Some(events);
        self
    }

    /// Swaps the memory store.
    pub fn with_memory(mut self, memory: Arc<dyn MemoryStore>) -> Self {
        self.memory = Some(memory);
        self
    }

    /// Swaps the context store.
    pub fn with_context(mut self, context: Arc<dyn ContextStore>) -> Self {
        self.context = Some(context);
        self
    }

    /// Swaps the tool provider.
    pub fn with_tools(mut self, tools: Arc<dyn ToolProvider>) -> Self {
        self.tools = Some(tools);
        self
    }

    /// Overrides the channel adapters (default: a single operator channel).
    pub fn with_channels(mut self, channels: Vec<Arc<dyn ChannelAdapter>>) -> Self {
        self.channels = Some(channels);
        self
    }

    /// Wires an agent economy (default: none).
    pub fn with_economy(mut self, economy: Arc<dyn AgentEconomy>) -> Self {
        self.economy = Some(economy);
        self
    }

    /// Swaps the approval gate (default: manifest `[policy]` gate).
    pub fn with_approvals(mut self, approvals: Arc<ManifestApprovalGate>) -> Self {
        self.approvals = Some(approvals);
        self
    }

    /// Convenience: build a fully fs-backed runtime with all Phase-1 defaults.
    pub async fn fs_defaults(
        home: impl Into<PathBuf>,
        manifest: CompanyManifest,
    ) -> Result<CompanyRuntime> {
        Self::new(home, manifest).build().await
    }

    /// Assembles the runtime, materializing `company.toml` and replaying the
    /// journal to rebuild the approval queue.
    pub async fn build(self) -> Result<CompanyRuntime> {
        let home = self.home;
        let id = self.id;

        let store: Arc<dyn CompanyStore> = self
            .store
            .unwrap_or_else(|| Arc::new(FsCompanyStore::new(home.clone())));
        let events: Arc<dyn EventLog> = self
            .events
            .unwrap_or_else(|| Arc::new(FsEventLog::new(home.clone())));
        let memory: Arc<dyn MemoryStore> = self
            .memory
            .unwrap_or_else(|| Arc::new(FsMemoryStore::new(home.clone())));
        let context: Arc<dyn ContextStore> = self
            .context
            .unwrap_or_else(|| Arc::new(FsContextStore::new(home.clone())));
        let tools: Arc<dyn ToolProvider> = self
            .tools
            .unwrap_or_else(|| Arc::new(StubToolProvider::new(self.manifest.tools.allow.clone())));
        let brain: Arc<dyn Brain> = self.brain.unwrap_or_else(|| Arc::new(EchoBrain::new()));
        let channels = self
            .channels
            .unwrap_or_else(|| vec![Arc::new(OperatorChannel::new()) as Arc<dyn ChannelAdapter>]);

        // Materialize the manifest so status/roster loads have a record to read.
        // `save` only writes company.toml + meta.json; the append-only ledger
        // file is left untouched, so an existing ledger survives a rebuild.
        let existing = store.load(&id).await?;
        let lifecycle = existing
            .as_ref()
            .map(|r| r.lifecycle.clone())
            .unwrap_or_else(|| "running".to_string());
        let ledger = existing.map(|r| r.ledger).unwrap_or_default();
        store
            .save(&CompanyRecord {
                id: id.clone(),
                manifest: self.manifest.clone(),
                ledger,
                lifecycle,
            })
            .await?;

        // Boot replay: load the journal and rehydrate parked approvals into the
        // gate so approvals survive a restart with their original ids.
        let journal = Arc::new(RuntimeJournal::new(
            Bundle::new(home.clone(), &id).journal_jsonl(),
        ));
        journal.load().await?;

        let gate = self
            .approvals
            .unwrap_or_else(|| Arc::new(ManifestApprovalGate::new(self.manifest.policy.clone())));
        for pending in journal.pending() {
            gate.rehydrate(pending.id, pending.effect, pending.at_millis);
        }
        let approvals: Arc<dyn ApprovalGate> = gate;

        Ok(CompanyRuntime::new(
            id,
            brain,
            store,
            events,
            memory,
            context,
            tools,
            channels,
            self.economy,
            approvals,
            journal,
        ))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn slugifies_display_names() {
        assert_eq!(company_id_from_name("Acme Co!").as_ref(), "acme-co");
        assert_eq!(company_id_from_name("  Widgets  ").as_ref(), "widgets");
        assert_eq!(company_id_from_name("***").as_ref(), "company");
    }
}
